//! Recording session orchestration.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::camera::{self, Camera, Mic};
use crate::synth::Synth;
use crate::compose::{Compositor, PipSpec, Scene};
use crate::screen::{self, ScreenCapture, ScreenConfig};
use crate::tcc::{self, Status};
use crate::util::*;
use crate::writer::{AudioLayout, Codec, PreviewFn, Sink, Stats, VideoSpec};

pub struct Shared {
    pub sink: Mutex<Option<Sink>>,
    pub error: Mutex<Option<String>>,
    /// Set when the session must end (error from a capture source/writer).
    pub stop: Arc<AtomicBool>,
}

impl Shared {
    pub fn with(&self, f: impl FnOnce(&mut Sink)) {
        if let Some(s) = self.sink.lock().unwrap().as_mut() {
            f(s);
        }
    }
    pub fn fail(&self, msg: String) {
        let mut e = self.error.lock().unwrap();
        if e.is_none() {
            *e = Some(msg);
        }
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Set by SIGINT/SIGTERM/SIGHUP in CLI mode.
static STOP: AtomicBool = AtomicBool::new(false);
/// Pause/resume requests (SIGUSR1/SIGUSR2 from `rec-rs pause`/`resume`).
pub static PAUSE_REQ: AtomicBool = AtomicBool::new(false);
pub static RESUME_REQ: AtomicBool = AtomicBool::new(false);
/// Save-replay request (SIGINFO from `rec-rs save-replay`).
pub static REPLAY_REQ: AtomicBool = AtomicBool::new(false);

extern "C" fn on_info_signal(_: libc::c_int) {
    REPLAY_REQ.store(true, Ordering::SeqCst);
}

pub extern "C" fn on_pause_signal(sig: libc::c_int) {
    if sig == libc::SIGUSR1 {
        PAUSE_REQ.store(true, Ordering::SeqCst);
    } else {
        RESUME_REQ.store(true, Ordering::SeqCst);
    }
}

pub fn install_pause_signals() {
    unsafe {
        libc::signal(libc::SIGUSR1, on_pause_signal as *const () as usize);
        libc::signal(libc::SIGUSR2, on_pause_signal as *const () as usize);
        libc::signal(libc::SIGINFO, on_info_signal as *const () as usize);
    }
}
static SIGNALS: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
    // A second Ctrl-C while finalizing aborts immediately.
    if SIGNALS.fetch_add(1, Ordering::SeqCst) >= 1 {
        unsafe { libc::_exit(130) };
    }
}

pub struct Options {
    pub output: PathBuf,
    pub size: (usize, usize),
    /// The capture target's native pixel size. Larger targets are captured
    /// at this size and scaled with `scale::FrameScaler` (ScreenCaptureKit's
    /// own downscale is soft).
    pub native: (usize, usize),
    pub fps: u32,
    pub codec: Codec,
    pub bitrate: u64,
    pub system_audio: bool,
    pub mic: Option<Option<String>>,
    /// Set when the requested mic isn't connected and the default is used.
    pub mic_warning: Option<String>,
    pub webcam: Option<Option<String>>,
    pub pip: PipSpec,
    pub cursor: bool,
    pub clicks: bool,
    pub duration: Option<f64>,
    pub quiet: bool,
    /// Replace the screen with a generated pattern (Some(noise)).
    pub test_pattern: Option<bool>,
    pub audio_layout: AudioLayout,
    /// [system, mic] fader levels at the start; `Session::set_volumes`
    /// changes them live.
    pub volumes: [crate::mixer::Volume; 2],
    pub mic_backend: MicBackend,
    /// Initial scene (only Screen without a webcam).
    pub scene: Scene,
    /// System audio offset in ms; None = the output device's latency.
    pub system_offset_ms: Option<f64>,
    /// Microphone offset in ms.
    pub mic_offset_ms: f64,
    /// Write the recording file (false: replay buffer / streams only).
    pub write: bool,
    /// Shared live encode for the replay buffer and streams.
    pub live: Option<Arc<crate::live::Live>>,
    /// Stream destinations (URL, key) to start with the session.
    pub streams: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum MicBackend {
    /// ScreenCaptureKit microphone capture (same clock/stream as the screen).
    Sck,
    /// AVCaptureSession audio input.
    Avcapture,
}

/// The privacy grants a session needs and doesn't have. It displays as the CLI's
/// error. The app writes its own banner, which points at its Request
/// Permissions button.
#[derive(Debug)]
pub struct MissingPermissions(pub Vec<&'static str>);

impl std::fmt::Display for MissingPermissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "missing permission(s): {}\n  run `rec permissions --request`, then enable Rec in System Settings > Privacy & Security",
            self.0.join(", ")
        )
    }
}

impl std::error::Error for MissingPermissions {}

pub fn check_permissions(o: &Options) -> Result<()> {
    let mut missing = vec![];
    if o.test_pattern.is_none() && tcc::screen_status() != Status::Granted {
        missing.push("Screen & System Audio Recording");
    }
    if o.mic.is_some() && tcc::mic_status() != Status::Granted {
        missing.push("Microphone");
    }
    if o.webcam.is_some() && tcc::camera_status() != Status::Granted {
        missing.push("Camera");
    }
    if !missing.is_empty() {
        return Err(MissingPermissions(missing).into());
    }
    Ok(())
}

/// A running capture session: recording to a file, or preview-only.
pub struct Session {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    pacer: Option<std::thread::JoinHandle<()>>,
    screen: Option<ScreenCapture>,
    synth: Option<Synth>,
    cam: Option<Camera>,
    av_mic: Option<Mic>,
    /// Keeps App Nap and timer coalescing off while capturing.
    activity: Option<SendBox<objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>>>,
    pub output: Option<PathBuf>,
    pub started: Instant,
    pub description: String,
}

/// How long a freshly started camera needs before its exposure is close to
/// settled (measured on a MacBook Pro camera: about 94% of the final
/// brightness after 1.5 s, 98% after 2 s).
const CAMERA_SETTLE_SECS: f64 = 2.0;

/// Size ScreenCaptureKit delivers: the native size when it is larger than
/// the output (scaled down here with a high-quality filter), else the output
/// size. `REC_SCALER=sck` keeps ScreenCaptureKit's own scaling.
fn capture_size(o: &Options) -> (usize, usize) {
    let (w, h) = o.size;
    let (nw, nh) = o.native;
    let sck = std::env::var("REC_SCALER").as_deref() == Ok("sck");
    if o.test_pattern.is_none() && !sck && nw * nh > w * h { (nw, nh) } else { (w, h) }
}

impl Session {
    /// Start capturing. `write` false = preview-only (nothing written).
    pub fn start(o: &Options, target: Option<screen::Target>, write: bool, preview: Option<PreviewFn>) -> Result<Session> {
        Self::start_with_camera(o, target, write, preview, None)
    }

    /// `start`, taking over a running camera (from `take_camera`) when it is
    /// the one this session wants: it keeps its settled exposure instead of
    /// restarting dark. Otherwise it is stopped.
    pub fn start_with_camera(
        o: &Options,
        target: Option<screen::Target>,
        write: bool,
        preview: Option<PreviewFn>,
        cam: Option<Camera>,
    ) -> Result<Session> {
        let (w, h) = o.size;
        // BGRA for the compositor, and for a window, whose transparent
        // rounded corners are filled before encoding (420v has no alpha).
        let window = matches!(target, Some(screen::Target::Window(_)));
        let pixel_format = if o.webcam.is_some() || window { PIX_BGRA } else { PIX_420V };
        let spec = VideoSpec { width: w, height: h, fps: o.fps, codec: o.codec, bitrate: o.bitrate, pixel_format };
        let compositor = o.webcam.as_ref().map(|_| Compositor::new(w, h, o.pip.clone()));
        let path = write.then_some(o.output.as_path());
        let mut sink = Sink::new(path, &spec, o.system_audio, o.mic.is_some(), o.audio_layout, o.volumes, compositor, o.scene, preview)?;
        if capture_size(o) != (w, h)
            && let Some(sc) = crate::scale::FrameScaler::new(w, h)
        {
            sink.set_scaler(sc);
        }
        if window && o.test_pattern.is_none() {
            sink.fill_window_corners();
        }
        sink.limit_secs(o.duration);
        // System audio from ScreenCaptureKit is captured when mixed and heard
        // one output latency later; the synthetic source needs no shift.
        let sys_ms = match (o.system_offset_ms, o.test_pattern) {
            (Some(ms), _) => ms,
            (None, Some(_)) => 0.0,
            (None, None) => crate::outlatency::output_latency().unwrap_or(0.0) * 1000.0,
        };
        if std::env::var_os("REC_DEBUG").is_some() {
            eprintln!("rec: debug: audio offsets: system {sys_ms:+.1} ms, mic {:+.1} ms", o.mic_offset_ms);
        }
        sink.set_audio_offsets([sys_ms / 1000.0, o.mic_offset_ms / 1000.0]);
        if let Some(l) = &o.live {
            l.begin_session(o.system_audio, o.mic.is_some());
            sink.set_live(l.clone());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared { sink: Mutex::new(Some(sink)), error: Mutex::new(None), stop: stop.clone() });
        let mut sess = Session {
            shared: shared.clone(),
            stop: stop.clone(),
            pacer: None,
            screen: None,
            synth: None,
            cam: None,
            av_mic: None,
            activity: None,
            output: write.then(|| o.output.clone()),
            started: Instant::now(),
            description: String::new(),
        };
        // On any start failure, tear down what was started and remove the file.
        match sess.start_sources(o, target, pixel_format, cam) {
            Ok(()) => {}
            Err(e) => {
                sess.stop_sources();
                if let Some(s) = shared.sink.lock().unwrap().take() {
                    let _ = s.finish(0.0);
                }
                if write {
                    let _ = std::fs::remove_file(&o.output);
                }
                return Err(e);
            }
        }
        // Pacer: emit constant-frame-rate video a little behind real time.
        let pacer_shared = shared.clone();
        let pacer_stop = stop.clone();
        let fps = o.fps;
        sess.pacer = Some(std::thread::spawn(move || {
            let period = Duration::from_secs_f64(1.0 / fps as f64 / 2.0);
            while !pacer_stop.load(Ordering::SeqCst) {
                let now = secs(host_now());
                // Plain threads have no autorelease pool; without one every
                // autoreleased CoreImage/AVFoundation object would leak.
                objc2::rc::autoreleasepool(|_| {
                    let mut failure = None;
                    pacer_shared.with(|s| {
                        s.emit_until(now - s.lag());
                        failure = s.failure().map(str::to_string);
                    });
                    if let Some(f) = failure {
                        pacer_shared.fail(format!("writer failed: {f}"));
                    }
                });
                std::thread::sleep(period);
            }
        }));
        use objc2_foundation::{NSActivityOptions, NSProcessInfo};
        let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
            NSActivityOptions::UserInitiated | NSActivityOptions::LatencyCritical,
            &ns("screen recording"),
        );
        sess.activity = Some(SendBox(token));
        sess.started = Instant::now();
        Ok(sess)
    }

    fn start_sources(&mut self, o: &Options, target: Option<screen::Target>, pixel_format: u32, cam: Option<Camera>) -> Result<()> {
        let (w, h) = o.size;
        let mut cam = cam;
        if let Some(sel) = &o.webcam {
            let reuse = cam.take_if(|c| Camera::resolve(sel.as_deref()).is_ok_and(|id| id == c.id));
            let c = match reuse {
                Some(c) => {
                    c.retarget(self.shared.clone());
                    c
                }
                None => Camera::start(sel.as_deref(), self.shared.clone())?,
            };
            // Start the file once the camera shows a picture and has had
            // time to settle its exposure, so it doesn't open on black or
            // dark frames (a preview just shows them).
            if self.output.is_some() && o.scene.uses_cam() {
                let settle = (CAMERA_SETTLE_SECS - c.started.elapsed().as_secs_f64()).max(0.0);
                self.shared.with(|s| s.wait_for_camera(settle));
            }
            self.cam = Some(c);
        }
        if let Some(c) = cam {
            c.stop();
        }
        let sck_mic = if o.mic_backend == MicBackend::Sck { o.mic.clone() } else { None };
        if o.mic_backend == MicBackend::Avcapture
            && let Some(sel) = &o.mic
        {
            self.av_mic = Some(Mic::start(sel.as_deref(), self.shared.clone())?);
        }
        match (o.test_pattern, target) {
            (Some(noise), _) => {
                self.synth = Some(Synth::start(w, h, o.fps, pixel_format, noise, o.system_audio, self.shared.clone()));
                self.description = format!("test pattern{}", if noise { " (noise)" } else { "" });
            }
            (None, Some(target)) => {
                self.description = target.describe();
                let (cw, ch) = capture_size(o);
                let cfg = ScreenConfig {
                    target,
                    width: cw,
                    height: ch,
                    fps: o.fps,
                    pixel_format,
                    cursor: o.cursor,
                    clicks: o.clicks,
                    system_audio: o.system_audio,
                    mic: sck_mic,
                };
                self.screen = Some(ScreenCapture::start(&cfg, self.shared.clone())?);
            }
            (None, None) => bail!("internal: no capture target"),
        }
        Ok(())
    }

    fn stop_sources(&mut self) -> Option<anyhow::Error> {
        let err = self.screen.take().and_then(|s| s.stop().err());
        if let Some(mut s) = self.synth.take() {
            s.stop();
        }
        if let Some(c) = self.cam.take() {
            c.stop();
        }
        if let Some(m) = self.av_mic.take() {
            m.stop();
        }
        err
    }

    /// Take the camera out of this session, still running, for the next
    /// one (`start_with_camera`).
    pub fn take_camera(&mut self) -> Option<Camera> {
        self.cam.take()
    }

    pub fn camera_name(&self) -> Option<&str> {
        self.cam.as_ref().map(|c| c.name.as_str())
    }

    /// True once a source or the writer failed (see `error`).
    pub fn failed(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }

    pub fn has_frames(&self) -> bool {
        let mut ok = false;
        self.shared.with(|s| ok = s.started());
        ok
    }

    pub fn pause(&self) -> bool {
        let mut ok = false;
        self.shared.with(|s| ok = s.pause(secs(host_now())));
        ok
    }

    pub fn resume(&self) -> bool {
        let mut ok = false;
        self.shared.with(|s| ok = s.resume(secs(host_now())));
        ok
    }

    pub fn is_paused(&self) -> bool {
        let mut p = false;
        self.shared.with(|s| p = s.is_paused());
        p
    }

    /// Recorded (output) seconds so far, excluding pauses.
    pub fn recorded_secs(&self) -> f64 {
        let mut t = 0.0;
        self.shared.with(|s| t = s.output_secs());
        t
    }

    /// Move or restyle the camera overlay live; also applies to the file.
    pub fn set_pip(&self, pip: PipSpec) {
        self.shared.with(|s| s.set_pip(pip));
    }

    /// Move the audio faders [system, mic] live: every track of the file
    /// and the live encode follow.
    pub fn set_volumes(&self, volumes: [crate::mixer::Volume; 2]) {
        self.shared.with(|s| s.set_volumes(volumes));
    }

    pub fn cam_aspect(&self) -> Option<f64> {
        let mut a = None;
        self.shared.with(|s| a = s.cam_aspect());
        a
    }

    pub fn set_scene(&self, scene: Scene) -> bool {
        let mut ok = false;
        self.shared.with(|s| ok = s.set_scene(scene));
        ok
    }

    /// (stats, peak levels [system, mic] since last call).
    pub fn poll(&self) -> (Stats, [f32; 2]) {
        let mut st = Stats::default();
        let mut lv = [0f32; 2];
        self.shared.with(|s| {
            st = s.stats.clone();
            lv = s.take_levels();
        });
        (st, lv)
    }

    /// Stop capture and finalize the file (if any). Blocks until written.
    pub fn finish(mut self) -> Result<Stats> {
        let end = secs(host_now());
        self.stop.store(true, Ordering::SeqCst);
        if let Some(p) = self.pacer.take() {
            let _ = p.join();
        }
        let stop_err = self.stop_sources();
        if let Some(t) = self.activity.take() {
            unsafe { objc2_foundation::NSProcessInfo::processInfo().endActivity(&t.0) };
        }
        let sink = self.shared.sink.lock().unwrap().take();
        let result = match sink {
            Some(s) => objc2::rc::autoreleasepool(|_| s.finish(end)),
            None => bail!("internal: sink missing"),
        };
        if let Some(e) = self.shared.error.lock().unwrap().take() {
            eprintln!("rec: error during recording: {e}");
        }
        if let Some(e) = stop_err {
            eprintln!("rec: warning: stopping screen capture: {e}");
        }
        result
    }
}

/// CLI recording: run until Ctrl-C/SIGTERM, `rec stop`, duration, or error.
pub fn run(o: Options, target: Option<screen::Target>) -> Result<Stats> {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
        libc::signal(libc::SIGHUP, on_signal as *const () as usize);
    }
    let mic_desc = match &o.mic {
        None => "off".to_string(),
        Some(None) => "default".to_string(),
        Some(Some(id)) => camera::microphones().into_iter().find(|m| &m.id == id).map(|m| m.name).unwrap_or(id.clone()),
    };
    // Before starting: a pause requested during camera startup still applies.
    install_pause_signals();
    let pidfile = pidfile_path();
    let _ = std::fs::write(&pidfile, format!("{}\n{}\n", std::process::id(), o.output.display()));
    let mut stream_ids = vec![];
    if let Some(l) = &o.live {
        for (url, key) in &o.streams {
            match crate::live::start_output(url, key) {
                Ok(out) => stream_ids.push(l.add_output(out)),
                Err(e) => {
                    let _ = std::fs::remove_file(&pidfile);
                    return Err(e);
                }
            }
        }
    }
    let sess = match Session::start(&o, target, o.write, None) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_file(&pidfile);
            return Err(e);
        }
    };
    let (w, h) = o.size;
    if !o.quiet {
        eprintln!(
            "rec: {} {} -> {} ({}x{} @ {}fps {:?}{}, system audio {}, mic {}, webcam {})",
            if o.write { "recording" } else { "capturing" },
            sess.description,
            if o.write { o.output.display().to_string() } else { "no file".into() },
            w,
            h,
            o.fps,
            o.codec,
            if o.codec == Codec::Prores { String::new() } else { format!(" {} Mb/s", o.bitrate / 1_000_000) },
            if o.system_audio { "on" } else { "off" },
            mic_desc,
            sess.camera_name().unwrap_or("off"),
        );
        eprintln!("rec: press Ctrl-C to stop");
    }


    let mut last_status = Instant::now();
    let mut warned_no_frames = false;
    let mut saves: Vec<std::thread::JoinHandle<Result<PathBuf>>> = vec![];
    while !STOP.load(Ordering::SeqCst) && !sess.failed() {
        std::thread::sleep(Duration::from_millis(20));
        // Requests wait for the first frame (the timeline starts there).
        if sess.has_frames() && PAUSE_REQ.swap(false, Ordering::SeqCst) && sess.pause() && !o.quiet {
            eprint!("\r\x1b[2Krec: paused (`rec-rs resume` to continue)\n");
        }
        if sess.has_frames() && RESUME_REQ.swap(false, Ordering::SeqCst) && sess.resume() && !o.quiet {
            eprint!("\r\x1b[2Krec: resumed\n");
        }
        if REPLAY_REQ.swap(false, Ordering::SeqCst)
            && let Some(l) = &o.live
        {
            let dir = o.output.parent().map(PathBuf::from).unwrap_or_default();
            match l.save_replay(dir.join(crate::settings::timestamped("replay", "mp4"))) {
                Ok(h) => saves.push(h),
                Err(e) => eprint!("\r\x1b[2Krec: replay: {e:#}\n"),
            }
        }
        for h in std::mem::take(&mut saves) {
            if h.is_finished() {
                match h.join() {
                    Ok(Ok(p)) => eprint!("\r\x1b[2Krec: replay saved {}\n", p.display()),
                    Ok(Err(e)) => eprint!("\r\x1b[2Krec: replay: {e:#}\n"),
                    Err(_) => eprint!("\r\x1b[2Krec: replay: writer panicked\n"),
                }
            } else {
                saves.push(h);
            }
        }
        let el = sess.started.elapsed().as_secs_f64();
        // --duration counts recorded time, so pauses extend the wall time
        // (and the wait for a camera to settle before the file starts).
        let rec_t = if sess.has_frames() { sess.recorded_secs() } else { 0.0 };
        if let Some(d) = o.duration
            && (rec_t >= d || (!sess.has_frames() && el >= d + CAMERA_SETTLE_SECS + 3.0))
        {
            break;
        }
        if !warned_no_frames && el > 3.0 && !sess.has_frames() {
            warned_no_frames = true;
            eprintln!("rec: warning: no screen frames after 3s (is Screen Recording permission granted?)");
        }
        if !o.quiet && last_status.elapsed() >= Duration::from_secs(1) {
            last_status = Instant::now();
            let (st, lv) = sess.poll();
            let mut live = String::new();
            if let Some(l) = &o.live {
                if let Some((secs, bytes)) = l.replay_stats() {
                    live += &format!("  replay {:.0}s {:.1} MB", secs, bytes as f64 / 1e6);
                }
                for (_, s) in l.statuses() {
                    live += &format!("  {} {} {:.1} Mb/s drop {}", s.name, s.state, s.bitrate / 1e6, s.dropped_frames);
                }
            }
            eprint!(
                "\r\x1b[2K  {}{}  video {} fr ({} dup, {} drop)  sys {} dB  mic {} dB{live}  ",
                clock(rec_t),
                if sess.is_paused() { " PAUSED" } else { "" },
                st.video_frames_out,
                st.video_dups,
                st.video_drops,
                if o.system_audio { db(lv[0]) } else { "   off".into() },
                if o.mic.is_some() { db(lv[1]) } else { "   off".into() },
            );
        }
    }
    if !o.quiet {
        eprintln!("\nrec: stopping...");
    }
    let result = sess.finish();
    if let Some(l) = &o.live {
        for id in stream_ids {
            l.remove_output(id);
        }
    }
    for h in saves {
        match h.join() {
            Ok(Ok(p)) => eprintln!("rec: replay saved {}", p.display()),
            Ok(Err(e)) => eprintln!("rec: replay: {e:#}"),
            Err(_) => {}
        }
    }
    let _ = std::fs::remove_file(&pidfile);
    result
}

pub fn clock(secs: f64) -> String {
    let s = secs as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) }
}

pub fn db(p: f32) -> String {
    if p <= 1e-5 { "  -inf".to_string() } else { format!("{:>6.1}", 20.0 * p.log10()) }
}

pub fn pidfile_path() -> PathBuf {
    std::env::temp_dir().join("rec.pid")
}

/// Send `sig` to the running recorder (SIGUSR1 pause, SIGUSR2 resume,
/// SIGINFO save replay).
pub fn signal_running(sig: libc::c_int) -> Result<()> {
    let text = std::fs::read_to_string(pidfile_path()).map_err(|_| anyhow::anyhow!("no recording in progress"))?;
    let pid: i32 = text.lines().next().unwrap_or("").trim().parse()?;
    if unsafe { libc::kill(pid, sig) } != 0 {
        bail!("no recording in progress (pid {pid} not running)");
    }
    Ok(())
}

/// Ask a running `rec record` to stop (like Ctrl-C) and wait for it to finish.
pub fn stop_running() -> Result<()> {
    let path = pidfile_path();
    let text = std::fs::read_to_string(&path).map_err(|_| anyhow::anyhow!("no recording in progress"))?;
    let mut lines = text.lines();
    let pid: i32 = lines.next().unwrap_or("").trim().parse()?;
    let file = lines.next().unwrap_or("").to_string();
    if unsafe { libc::kill(pid, 0) } != 0 {
        let _ = std::fs::remove_file(&path);
        bail!("no recording in progress (stale pidfile removed)");
    }
    unsafe { libc::kill(pid, libc::SIGINT) };
    let deadline = Instant::now() + Duration::from_secs(90);
    // Done when the recorder removes its pidfile (file finalized) or exits.
    while unsafe { libc::kill(pid, 0) } == 0 && path.exists() {
        if Instant::now() > deadline {
            bail!("recorder (pid {pid}) did not exit within 90s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("{file}");
    Ok(())
}
